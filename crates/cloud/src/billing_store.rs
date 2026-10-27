//! The durable billing seam: the [`BillingStore`] trait plus its in-memory
//! implementation and the additive SQLite implementation over the SAME
//! [`crate::store::SqliteControlPlaneStore`] database and migration ladder
//! (its migration v2 — the crate-owned next `user_version` — and its
//! migration v6, which rebuilds `usage_event.task_id` from the old lossy
//! signed INTEGER projection to the reversible fixed-width text encoding
//! [`crate::billing::task_id_text`]).
//!
//! Append-only enforcement lives here: usage events and credit entries are
//! INSERT-only (no UPDATE/DELETE surface exists), ingestion is idempotent
//! per `(organization, source_key)`, and every credit mutation runs in ONE
//! immediate transaction that re-derives the balance before the insert, so
//! two racing consumes can never jointly overdraw the account and a refund
//! can never exceed what its referenced consume/settle actually spent.
//!
//! Task identity: the `task_id` column is TEXT holding exactly
//! [`crate::billing::task_id_text`]`(id)` (16 lowercase hex digits, a
//! bijection over the full u64 domain). Writes encode through that helper
//! and task-filtered queries compare against the same encoding, so
//! `usage_events_of_task` can never alias two ids that the old
//! `task_id.min(i64::MAX as u64) as i64` projection collapsed together.
//!
//! Amount projection: `credit_entry.amount_micro` is a signed `INTEGER`
//! column whose domain is bounded by
//! [`crate::billing::MAX_CREDIT_AMOUNT_MICRO`] (`i64::MAX` micro-units).
//! Writes therefore store the amount EXACTLY (no `.min(...)` clamp, checked
//! conversion only) and every read decodes the column and the payload
//! together, refusing typed when they disagree — a legacy row written by the
//! old clamping writer is surfaced, never silently reinterpreted as its
//! clamped sentinel.

use std::collections::BTreeMap;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use faktor_core::billing::BillingPeriod;

use crate::billing::{
    checked_append_domain, checked_ledger_sum, fold_usage, task_id_from_text, task_id_text,
    BillingAccount, CreditBalance, CreditEntry, CreditKind, CreditLedgerError, InFlightTxn,
    SpendCategory, Subscription, TaskUsage, UsageEvent, UsageFold, UsageTotals, UsageUnit,
};
use crate::error::ControlPlaneError;
use crate::ids::{BillingAccountId, InFlightTxnId, OrganizationId};
use crate::store::{CloudStoreError, SqliteControlPlaneStore};

/// The SQL schema of the billing domain (migration v2 of the control-plane
/// ladder). Append-only by construction: no UPDATE/DELETE statement names
/// `usage_event` or `credit_entry` outside the store's own read paths.
///
/// `usage_event.task_id` is created as `INTEGER` here for historical shape
/// only: migration [`BILLING_TASK_ID_TEXT_SCHEMA_V6`] rebuilds it as the
/// reversible fixed-width TEXT encoding before any query runs, fresh
/// databases included (see [`crate::billing::task_id_text`]).
#[path = "billing_store_period.rs"]
mod period;
pub use period::BILLING_PERIOD_SCHEMA_V9;
use period::*;

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

/// The SQL half of migration v6 of the control-plane ladder: rebuild
/// `usage_event.task_id` from the lossy signed INTEGER projection to the
/// reversible fixed-width TEXT encoding ([`crate::billing::task_id_text`]).
///
/// The old projection was `task_id.min(i64::MAX as u64) as i64`: every id
/// `>= i64::MAX` aliased onto the same indexed value, so a task-filtered
/// query could return another task's ledger rows. The rebuild cannot be
/// completed in SQL alone: SQLite JSON functions expose an integer above
/// `i64::MAX` only as an inexact REAL, and the legacy payload is the sole
/// authority for such an id. This SQL therefore only STAGES the rebuild —
/// every row gets a syntactically valid placeholder and its legacy INTEGER
/// value is carried in `usage_event_v6_task_id_carry` — and
/// [`finalize_task_id_text_migration`] resolves each row's exact id in Rust
/// inside the SAME migration transaction, then drops the carry table. The
/// placeholder is never visible to a committed database; if any row cannot
/// be recovered exactly the whole migration transaction rolls back and the
/// pre-v6 database (and its rows) stay untouched.
pub const BILLING_TASK_ID_TEXT_SCHEMA_V6: &str = "
     CREATE TABLE usage_event_v6 (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        event_seq INTEGER NOT NULL,
        source_key TEXT NOT NULL,
        billing_account_id TEXT NOT NULL,
        task_id TEXT NOT NULL
            CONSTRAINT usage_event_task_id_text CHECK (
                length(task_id) = 16
                AND task_id NOT GLOB '*[^0-9a-f]*'
            ),
        run_id TEXT NOT NULL,
        attempt_id TEXT NOT NULL,
        category TEXT NOT NULL,
        correction_of TEXT,
        occurred_at_ms INTEGER NOT NULL,
        reconciliation_state TEXT NOT NULL,
        payload TEXT NOT NULL,
        UNIQUE (organization_id, source_key)
     );
     CREATE TABLE usage_event_v6_task_id_carry (
        event_id TEXT PRIMARY KEY,
        legacy_task_id INTEGER NOT NULL
     );
     INSERT INTO usage_event_v6
        (id, organization_id, event_seq, source_key, billing_account_id, task_id,
         run_id, attempt_id, category, correction_of, occurred_at_ms,
         reconciliation_state, payload)
     SELECT id, organization_id, event_seq, source_key, billing_account_id,
            '0000000000000000',
            run_id, attempt_id, category, correction_of, occurred_at_ms,
            reconciliation_state, payload
     FROM usage_event;
     INSERT INTO usage_event_v6_task_id_carry (event_id, legacy_task_id)
     SELECT id, task_id FROM usage_event;
     DROP TABLE usage_event;
     ALTER TABLE usage_event_v6 RENAME TO usage_event;
     CREATE INDEX IF NOT EXISTS idx_usage_event_org_seq
        ON usage_event(organization_id, event_seq);
     CREATE INDEX IF NOT EXISTS idx_usage_event_org_task
        ON usage_event(organization_id, task_id, event_seq);
     CREATE INDEX IF NOT EXISTS idx_usage_event_correction
        ON usage_event(organization_id, correction_of);
";

/// The staging table [`BILLING_TASK_ID_TEXT_SCHEMA_V6`] carries legacy
/// INTEGER task ids in until [`finalize_task_id_text_migration`] resolves
/// them; its presence marks the rebuild as not yet finalized.
const TASK_ID_TEXT_CARRY_TABLE: &str = "usage_event_v6_task_id_carry";
/// Page size of the v6 finalizer (bounded everything: the migration never
/// holds an unbounded row set in memory).
const TASK_ID_TEXT_FINALIZE_PAGE: i64 = 256;

/// Resolve the staged v6 rebuild exactly. For every carried row: read the
/// original id from the JSON payload (the authority the read paths already
/// return), verify it against the carried legacy projection, write the
/// reversible [`crate::billing::task_id_text`] encoding, and finally drop
/// the carry table. Runs inside the migration transaction, so a refusal
/// leaves the database exactly at v5.
///
/// Refuses typed (never guesses) when a row cannot be recovered exactly:
///
/// - the legacy stored value is not a non-zero id;
/// - the payload is unreadable or carries no exact u64 `task_id` (ids above
///   `i64::MAX` cannot be parsed out of JSON by SQL, which is why this runs
///   in Rust);
/// - the payload id is 0 (never a legal event) or the two sources
///   disagree. A carried value below `i64::MAX` was stored exactly, so a
///   disagreement is corruption. A carried `i64::MAX` is the one
///   intentionally lossy case (every original id `>= i64::MAX` clamped onto
///   it), so it is accepted when the payload id is `>= i64::MAX` and
///   refused when the payload id is lower.
///
/// Idempotent: a no-op when the carry table is absent (already finalized or
/// never staged), so it is safe to call on every open.
pub(crate) fn finalize_task_id_text_migration(conn: &Connection) -> Result<(), BillingStoreError> {
    let staged: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![TASK_ID_TEXT_CARRY_TABLE],
            |r| r.get(0),
        )
        .optional()
        .map_err(backend)?;
    if staged.is_none() {
        return Ok(());
    }
    let mut after = String::new();
    loop {
        let mut stmt = conn
            .prepare(
                "SELECT carry.event_id, carry.legacy_task_id, event.payload
                 FROM usage_event_v6_task_id_carry carry
                 JOIN usage_event event ON event.id = carry.event_id
                 WHERE carry.event_id > ?1
                 ORDER BY carry.event_id LIMIT ?2",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![after, TASK_ID_TEXT_FINALIZE_PAGE], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        if rows.is_empty() {
            break;
        }
        after = rows.last().map(|row| row.0.clone()).unwrap_or_default();
        for (event_id, legacy_task_id, payload) in rows {
            let task_id = resolve_legacy_task_id(&event_id, legacy_task_id, &payload)?;
            conn.execute(
                "UPDATE usage_event SET task_id = ?1 WHERE id = ?2",
                params![crate::billing::task_id_text(task_id), event_id],
            )
            .map_err(backend)?;
        }
    }
    conn.execute("DROP TABLE usage_event_v6_task_id_carry", [])
        .map_err(backend)?;
    Ok(())
}

/// One legacy row's exact recovery (see [`finalize_task_id_text_migration`]
/// for the refusal rules). Extracted so every refusal names the event id.
fn resolve_legacy_task_id(
    event_id: &str,
    legacy_task_id: i64,
    payload: &str,
) -> Result<u64, BillingStoreError> {
    let refuse = |reason: String| {
        BillingStoreError::Malformed(format!(
            "usage_event {event_id}: {reason}; refusing to guess the task identity \
             (operator verification required)"
        ))
    };
    if legacy_task_id < 1 {
        return Err(refuse(format!(
            "legacy stored task id {legacy_task_id} is not a valid non-zero id"
        )));
    }
    let payload: serde_json::Value = serde_json::from_str(payload).map_err(|e| {
        refuse(format!(
            "payload is unreadable ({e}), so the exact task id cannot be recovered"
        ))
    })?;
    let task_id = payload
        .get("task_id")
        .and_then(|value| value.as_u64())
        .ok_or_else(|| refuse("payload carries no exact u64 task_id".to_string()))?;
    if task_id == 0 {
        return Err(refuse("payload task id is 0".to_string()));
    }
    if legacy_task_id < i64::MAX {
        if task_id != legacy_task_id as u64 {
            return Err(refuse(format!(
                "payload task id {task_id} disagrees with the exact stored id {legacy_task_id}"
            )));
        }
    } else if task_id < i64::MAX as u64 {
        return Err(refuse(format!(
            "the stored id is the clamped sentinel i64::MAX but payload task id {task_id} is lower, \
             so the original id cannot be recovered unambiguously"
        )));
    }
    Ok(task_id)
}

/// One stored usage event with its immutable durable order (`event_seq`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredUsageEvent {
    pub event_seq: i64,
    pub event: UsageEvent,
    /// The durable billing period this event's settlement time fell in.
    pub period_id: String,
}

/// The SQL schema of the durable billing-report schedule (migration v4 of
/// the control-plane ladder). Append-only in spirit: rows advance through
/// their open -> reported/failed/skipped states, and a terminal period is
/// never re-opened (the runner never re-opens it), so a period can never be
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

/// The SQL schema of the durable billing AGGREGATE projections (migration
/// v8 of the control-plane ladder): one usage-total row per organization,
/// one usage row per `(organization, task, run)` bucket, and one credit
/// balance row per organization. They are PROJECTIONS of the append-only
/// ledgers, maintained in the same transaction as every append (and lazily
/// rebuilt from the ledger rows when absent/stale), so an entitlement check
/// or a report no longer folds the whole ledger on every request.
///
/// Counters are stored as decimal TEXT, not SQLite INTEGER: the pure folds
/// aggregate in `u64` and a text column keeps the projection EXACT for every
/// value the fold accepts (an INTEGER column would silently become REAL or
/// refuse above `i64::MAX`). `watermark` is the last `event_seq` /
/// `entry_seq` applied, so a projection can be caught up incrementally after
/// a crash, an upgrade from v7, or a restored backup.
pub const BILLING_ROLLUP_SCHEMA_V8: &str = "
     CREATE TABLE IF NOT EXISTS billing_usage_total (
        organization_id TEXT PRIMARY KEY,
        input_tokens TEXT NOT NULL,
        output_tokens TEXT NOT NULL,
        cache_read_tokens TEXT NOT NULL,
        cache_write_tokens TEXT NOT NULL,
        reasoning_tokens TEXT NOT NULL,
        provider_cost_micro TEXT NOT NULL,
        managed_cost_micro TEXT NOT NULL,
        byok_cost_micro TEXT NOT NULL,
        events TEXT NOT NULL,
        corrected_events TEXT NOT NULL,
        watermark INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS billing_usage_task (
        organization_id TEXT NOT NULL,
        task_id TEXT NOT NULL,
        run_id TEXT NOT NULL,
        input_tokens TEXT NOT NULL,
        output_tokens TEXT NOT NULL,
        cache_read_tokens TEXT NOT NULL,
        cache_write_tokens TEXT NOT NULL,
        reasoning_tokens TEXT NOT NULL,
        provider_cost_micro TEXT NOT NULL,
        managed_cost_micro TEXT NOT NULL,
        byok_cost_micro TEXT NOT NULL,
        events TEXT NOT NULL,
        corrected_events TEXT NOT NULL,
        PRIMARY KEY (organization_id, task_id, run_id)
     );
     CREATE TABLE IF NOT EXISTS billing_credit_rollup (
        organization_id TEXT PRIMARY KEY,
        granted_micro TEXT NOT NULL,
        consumed_micro TEXT NOT NULL,
        refunded_micro TEXT NOT NULL,
        held_micro TEXT NOT NULL,
        pending_consumes TEXT NOT NULL,
        watermark INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_usage_event_correction_winner
        ON usage_event(organization_id, correction_of, occurred_at_ms DESC, id DESC);
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
    /// Permanently skipped: the schedule was down long enough that this
    /// period fell outside the bounded catch-up window. The row IS the
    /// durable audit record (`last_error` names the skipped range, and
    /// `skipped_at_ms` records when the decision was made) — a missed
    /// period is never silently ignored.
    Skipped,
}

impl ReportPeriodStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            ReportPeriodStatus::Open => "open",
            ReportPeriodStatus::Reported => "reported",
            ReportPeriodStatus::Failed => "failed",
            ReportPeriodStatus::Skipped => "skipped",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "open" => Some(ReportPeriodStatus::Open),
            "reported" => Some(ReportPeriodStatus::Reported),
            "failed" => Some(ReportPeriodStatus::Failed),
            "skipped" => Some(ReportPeriodStatus::Skipped),
            _ => None,
        }
    }

    /// Whether the status is terminal (a terminal period is never
    /// re-opened or re-sent).
    pub const fn is_terminal(self) -> bool {
        !matches!(self, ReportPeriodStatus::Open)
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
    /// When a [`ReportPeriodStatus::Skipped`] audit decision was made
    /// (never set for any other status).
    #[serde(default)]
    pub skipped_at_ms: Option<i64>,
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
        "settle refused: consume {reference} already has a refund; a released hold is never \
         settled (nothing was written)"
    )]
    SettleAfterRefund { reference: String },
    #[error(
        "refund refused: consume {reference} is still pending; only its full remaining hold of \
         {refundable} microUSD can be released (requested {requested}; nothing was written)"
    )]
    PendingHoldRefundNotFull {
        reference: String,
        refundable: u64,
        requested: u64,
    },
    #[error(
        "refund refused: {requested} microUSD exceeds the {refundable} microUSD left of consume \
         {reference} (nothing was written)"
    )]
    RefundExceedsConsumed {
        reference: String,
        refundable: u64,
        requested: u64,
    },
    #[error(
        "credit reference {reference} belongs to billing account {expected}, not {actual}: a \
         settle/refund can never cross accounts (nothing was written)"
    )]
    ReferenceAccountMismatch {
        reference: String,
        expected: String,
        actual: String,
    },
    #[error(
        "consume {reference} is already closed (settled or refunded); a replayed hold can never reopen it (nothing was written)"
    )]
    ConsumeAlreadyClosed { reference: String },
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
    /// A monetary aggregate left the `u64` domain or the ledger violates its
    /// own invariants: typed, never saturated (a saturated balance would hide
    /// debt from every guard).
    #[error(transparent)]
    Ledger(#[from] CreditLedgerError),
}

impl From<CloudStoreError> for BillingStoreError {
    fn from(e: CloudStoreError) -> Self {
        match e {
            CloudStoreError::Backend(m) => BillingStoreError::Backend(m),
            CloudStoreError::Malformed(m) => BillingStoreError::Malformed(m),
            CloudStoreError::Conflict(m) => BillingStoreError::Malformed(m),
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
            // A typed ledger refusal (overflow / corrupt invariant) stays
            // TYPED at the control-plane boundary: it is never retryable and
            // never flattened into a generic backend failure.
            BillingStoreError::Ledger(e) => ControlPlaneError::Ledger(e),
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
    /// Every event of one organization with `task_id`, ascending (bounded
    /// by `limit`). Both implementations match the EXACT id: SQLite compares
    /// the canonical [`crate::billing::task_id_text`] encoding of the
    /// column, the memory store the raw `u64` — no projection may alias two
    /// ids.
    fn usage_events_of_task(
        &self,
        organization: &OrganizationId,
        task_id: u64,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredUsageEvent>, BillingStoreError>;

    /// The organization's usage fold (org totals plus per-task buckets).
    /// Durable implementations serve it from the maintained rollup, so the
    /// cost is O(tasks) — never one full-ledger scan per request.
    fn usage_fold(&self, organization: &OrganizationId) -> Result<UsageFold, BillingStoreError>;

    /// The organization's usage TOTALS only (the admission/report path): one
    /// row, no per-task materialization.
    fn usage_totals(&self, organization: &OrganizationId)
        -> Result<UsageTotals, BillingStoreError>;

    /// The organization's usage fold for ONE billing period: only the events
    /// whose durable settlement time fell in `period_id` (corrections stay in
    /// their base event's period). Per-task buckets are keyed by
    /// `(organization, period, task, run)`.
    fn usage_fold_in_period(
        &self,
        organization: &OrganizationId,
        period_id: &str,
    ) -> Result<UsageFold, BillingStoreError>;

    /// The organization's usage TOTALS for ONE billing period (the quota
    /// gate's numerator: limits evaluate the CURRENT period only).
    fn usage_totals_in_period(
        &self,
        organization: &OrganizationId,
        period_id: &str,
    ) -> Result<UsageTotals, BillingStoreError>;

    /// Persist the deterministic period configuration (subscription anchor +
    /// plan period length) usage events are stamped from.
    fn put_period_config(
        &self,
        organization: &OrganizationId,
        anchor_ms: i64,
        period_ms: i64,
    ) -> Result<(), BillingStoreError>;

    /// The stored period configuration, if one was written.
    fn period_config(
        &self,
        organization: &OrganizationId,
    ) -> Result<Option<(i64, i64)>, BillingStoreError>;

    /// The highest settlement-outbox id already projected into this
    /// organization's usage ledger (0 = nothing projected yet).
    fn outbox_watermark(&self, organization: &OrganizationId) -> Result<i64, BillingStoreError>;

    /// Advance the settlement-outbox projection watermark monotonically.
    fn advance_outbox_watermark(
        &self,
        organization: &OrganizationId,
        last_outbox_id: i64,
    ) -> Result<(), BillingStoreError>;
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

    /// Claim one idempotency key for a non-ledger commercial mutation with
    /// its canonical request body. The first claim is `Fresh`; the same
    /// request replays; a different request under the same key conflicts.
    /// Both bodies are bounded by [`MAX_BILLING_MUTATION_BODY`].
    fn claim_billing_mutation(
        &self,
        organization: &OrganizationId,
        key: &str,
        request: &str,
        at_ms: i64,
    ) -> Result<BillingMutationClaim, BillingStoreError>;
    /// The recorded response of a claimed mutation (None while the mutation
    /// is still in flight or after a crash).
    fn billing_mutation_response(
        &self,
        organization: &OrganizationId,
        key: &str,
    ) -> Result<Option<String>, BillingStoreError>;
    /// Record the response of a claimed mutation (idempotent overwrite of the
    /// same bounded response).
    fn finish_billing_mutation(
        &self,
        organization: &OrganizationId,
        key: &str,
        response: &str,
    ) -> Result<(), BillingStoreError>;

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

/// Bound on the canonical request/response body recorded for one idempotent
/// commercial mutation (subscription change/cancel). The payload is small by
/// construction; the bound keeps the durable idempotency row bounded.
pub const MAX_BILLING_MUTATION_BODY: usize = 64 * 1024;

/// Bound on the durable idempotency keys retained per organization. The
/// table is append-only keyed by caller-chosen keys, so an owner could
/// otherwise grow it without bound; the newest keys are kept (older keys lose
/// their replay record, which is the documented bound).
pub const MAX_BILLING_MUTATIONS_PER_ORG: usize = 1_024;

/// The outcome of claiming one idempotency key for a non-ledger commercial
/// mutation (the subscription change/cancel route). The key is scoped to the
/// organization, so one tenant can never collide with (or probe) another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BillingMutationClaim {
    /// First sighting: the caller performs the mutation and records the
    /// response.
    Fresh,
    /// The same key + byte-identical canonical request replays the recorded
    /// response (or, when a crash left no response, the caller recomputes the
    /// naturally idempotent mutation).
    Replay { response: Option<String> },
    /// The same key was used for a DIFFERENT request: refused, nothing is
    /// mutated.
    Conflict { request: String },
}

/// The lazy SQL schema of the commercial mutation idempotency table. It is
/// created on first use (independent of the control-plane migration ladder),
/// so an existing billing database gains it without a version bump and a
/// fresh one gains it on the first mutating request.
const BILLING_MUTATION_SCHEMA: &str = "
     CREATE TABLE IF NOT EXISTS billing_mutation (
        organization_id TEXT NOT NULL,
        idempotency_key TEXT NOT NULL,
        request TEXT NOT NULL,
        response TEXT,
        created_ms INTEGER NOT NULL,
        PRIMARY KEY (organization_id, idempotency_key)
     );
";

/// Enforce the credit-amount domain bound at the persistence boundary
/// (defense in depth: the service validates too, but no store write may ever
/// project an out-of-domain amount into the ledger). The refusal is typed and
/// names the field and the limit; both backends refuse identically.
fn validate_credit_amount(entry: &CreditEntry) -> Result<(), BillingStoreError> {
    crate::billing::validate_credit_amount_micro(entry.amount_micro)
        .map_err(|e| BillingStoreError::Malformed(e.to_string()))
}

// ------------------------------------------------------------- in-memory

#[path = "billing_store_memory.rs"]
mod memory;
use memory::MemBilling;
pub use memory::MemoryBillingStore;

fn report_key(organization: &str, period: &str) -> String {
    format!("{organization}\0{period}")
}

/// Prune one organization's report rows to the newest `bound` (by first
/// sighting, then period): the schedule stays bounded over an unbounded
/// lifetime. Terminal rows are pruned first so an open period or a skipped
/// audit row is never silently dropped while an old terminal row survives.
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
                row.status == ReportPeriodStatus::Open || row.status == ReportPeriodStatus::Skipped,
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

/// Page limits arrive as `usize` but bind into SQLite's signed range: a
/// limit above `i64::MAX` is refused typed instead of wrapping negative
/// (SQLite reads a negative LIMIT as unbounded).
fn checked_page_limit(limit: usize) -> Result<i64, BillingStoreError> {
    i64::try_from(limit).map_err(|_| {
        BillingStoreError::Malformed(format!("page limit {limit} exceeds SQLite signed range"))
    })
}

/// One matched `credit_entry` idempotency row: (id, kind, stored
/// `amount_micro` column, reference, billing_account_id, usage_event_id,
/// payload). The payload is carried so the identity comparison uses the
/// EXACT amount authority, never the signed column projection.
type CreditEntryKeyRow = (
    String,
    String,
    i64,
    Option<String>,
    String,
    Option<String>,
    String,
);

impl SqliteControlPlaneStore {
    /// Run one credit append inside an IMMEDIATE transaction: the balance is
    /// re-derived from the durable rows and the guards applied before the
    /// insert, so concurrent writers can never overdraw or over-refund.
    fn credit_append_tx(
        conn: &mut Connection,
        entry: &CreditEntry,
    ) -> Result<CreditAppend, BillingStoreError> {
        // The amount domain bound is enforced BEFORE the transaction opens:
        // an out-of-domain amount is refused typed and writes nothing.
        validate_credit_amount(entry)?;
        // Enforce the full entry shape at the same seam: a settle/refund
        // without a reference (or other malformed field) is a typed refusal,
        // never an `expect` panic inside the append guards.
        entry
            .validate()
            .map_err(|error| BillingStoreError::Malformed(error.to_string()))?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        if let Some(key) = &entry.idempotency_key {
            let existing: Option<CreditEntryKeyRow> = tx
                .query_row(
                    "SELECT id, kind, amount_micro, reference, billing_account_id, usage_event_id,
                            payload
                     FROM credit_entry
                     WHERE organization_id = ?1 AND idempotency_key = ?2",
                    params![entry.organization.as_str(), key],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                        ))
                    },
                )
                .optional()
                .map_err(backend)?;
            if let Some((id, kind, amount, reference, account, usage_event, payload)) = existing {
                // The payload amount is the authority (and a legacy clamped
                // row is refused typed here too, before any comparison).
                let stored = decode_credit_row(&id, amount, &payload)?;
                let same = kind == entry.kind.as_str()
                    && stored.amount_micro == entry.amount_micro
                    && reference.as_deref() == entry.reference.as_ref().map(|r| r.as_str())
                    && account == entry.billing_account_id.as_str()
                    && usage_event.as_deref() == entry.usage_event_id.as_ref().map(|u| u.as_str());
                if same {
                    // A replay of a record-before-call hold is only a replay
                    // while the hold is still open: a settle/refund closed it,
                    // and handing back a hold token would let the caller
                    // dispatch an unpaid provider call.
                    if stored.kind == CreditKind::Consume
                        && (credit_entry_for_reference(
                            &tx,
                            &entry.organization,
                            stored.id.as_str(),
                            CreditKind::Settle,
                        )?
                        .is_some()
                            || credit_entry_for_reference(
                                &tx,
                                &entry.organization,
                                stored.id.as_str(),
                                CreditKind::Refund,
                            )?
                            .is_some())
                    {
                        return Err(BillingStoreError::Credit(
                            CreditAppendRefusal::ConsumeAlreadyClosed {
                                reference: stored.id.as_str().to_string(),
                            },
                        ));
                    }
                    tx.commit().map_err(backend)?;
                    return Ok(CreditAppend::Duplicate);
                }
                return Err(BillingStoreError::Credit(
                    CreditAppendRefusal::IdempotencyConflict(key.clone()),
                ));
            }
        }
        let mut balance = ensure_credit_rollup_tx(&tx, &entry.organization)?;
        match entry.kind {
            CreditKind::Grant => {}
            CreditKind::Consume => {
                let available = balance.balance_micro()?;
                if entry.amount_micro > available {
                    return Err(BillingStoreError::Credit(
                        CreditAppendRefusal::InsufficientCredits {
                            available,
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
                let target = credit_entry_by_id(&tx, &entry.organization, reference.as_str())?;
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
                if target.billing_account_id != entry.billing_account_id {
                    return Err(BillingStoreError::Credit(
                        CreditAppendRefusal::ReferenceAccountMismatch {
                            reference: reference.as_str().to_string(),
                            expected: target.billing_account_id.as_str().to_string(),
                            actual: entry.billing_account_id.as_str().to_string(),
                        },
                    ));
                }
                let settle = credit_entry_for_reference(
                    &tx,
                    &entry.organization,
                    reference.as_str(),
                    CreditKind::Settle,
                )?;
                let settle_amount = settle.as_ref().map(|row| row.amount_micro);
                if entry.kind == CreditKind::Settle {
                    if settle.is_some() {
                        return Err(BillingStoreError::Credit(
                            CreditAppendRefusal::AlreadySettled {
                                reference: reference.as_str().to_string(),
                            },
                        ));
                    }
                    // A released hold is never settled: refunding a pending
                    // consume cancels it, and charging the actual after that
                    // would let the organization spend credits it already got
                    // back (a double-spend/credit-minting hole).
                    if credit_entry_for_reference(
                        &tx,
                        &entry.organization,
                        reference.as_str(),
                        CreditKind::Refund,
                    )?
                    .is_some()
                    {
                        return Err(BillingStoreError::Credit(
                            CreditAppendRefusal::SettleAfterRefund {
                                reference: reference.as_str().to_string(),
                            },
                        ));
                    }
                    let delta = entry.amount_micro.saturating_sub(target.amount_micro);
                    // The pending hold is ALREADY inside `balance_micro()`
                    // (an unsettled consume counts as consumed at its held
                    // amount), so only the delta above it must be covered.
                    // Adding `target.amount_micro` back would double-count
                    // the hold and let a settle spend free + hold.
                    let available = balance.balance_micro()?;
                    if delta > available {
                        return Err(BillingStoreError::Credit(
                            CreditAppendRefusal::InsufficientCredits {
                                available,
                                requested: delta,
                            },
                        ));
                    }
                } else {
                    let spent = settle_amount.unwrap_or(target.amount_micro);
                    // Checked: a ledger whose refunds exceed the spend is
                    // refused typed, never masked to a zero refundable
                    // amount.
                    let mut refunded: u64 = 0;
                    for row in credit_entries_for_reference(
                        &tx,
                        &entry.organization,
                        reference.as_str(),
                        CreditKind::Refund,
                    )? {
                        refunded =
                            checked_ledger_sum("refunded_micro", refunded, row.amount_micro)?;
                    }
                    let refundable =
                        spent
                            .checked_sub(refunded)
                            .ok_or_else(|| CreditLedgerError::Corrupt {
                                detail: format!(
                                    "consume {} has {refunded} microUSD refunded against {spent} \
                                 microUSD spent",
                                    reference.as_str()
                                ),
                            })?;
                    if settle.is_none() {
                        // A pending consume is cancelled only by releasing its
                        // FULL remaining hold: a partial refund would leave a
                        // half-released hold that a later settle could charge
                        // again (free managed spend).
                        if entry.amount_micro != refundable {
                            return Err(BillingStoreError::Credit(
                                CreditAppendRefusal::PendingHoldRefundNotFull {
                                    reference: reference.as_str().to_string(),
                                    refundable,
                                    requested: entry.amount_micro,
                                },
                            ));
                        }
                    } else if entry.amount_micro > refundable {
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
        // The entry's own contribution must not leave the u64 domain: refused
        // typed before the write (the transaction rolls back, nothing is
        // written).
        checked_append_domain(entry, &balance)?;
        let seq: i64 = tx
            .query_row(
                "INSERT INTO billing_credit_seq (organization_id, next_seq) VALUES (?1, 1)
                 ON CONFLICT(organization_id) DO UPDATE SET next_seq = next_seq + 1
                 RETURNING next_seq",
                params![entry.organization.as_str()],
                |r| r.get(0),
            )
            .map_err(backend)?;
        // The domain bound (validated above) makes the signed projection
        // EXACT. The conversion is still checked, never `as`: no future path
        // can silently reintroduce a clamp into the monetary column.
        let amount_i64 = i64::try_from(entry.amount_micro).map_err(|_| {
            BillingStoreError::Malformed(format!(
                "credit entry {} amount_micro {} exceeds the durable i64::MAX limit {}",
                entry.id.as_str(),
                entry.amount_micro,
                crate::billing::MAX_CREDIT_AMOUNT_MICRO
            ))
        })?;
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
                amount_i64,
                entry.occurred_at_ms,
                encode(entry)?,
            ],
        )
        .map_err(backend)?;
        apply_credit_entry(&tx, &entry.organization, &mut balance, entry)?;
        write_credit_projection(&tx, &entry.organization, &balance, seq)?;
        tx.commit().map_err(backend)?;
        Ok(CreditAppend::Appended)
    }
}

/// One credit entry by its durable id (tenant-scoped), decoded with the
/// payload/column amount cross-check.
fn credit_entry_by_id(
    conn: &Connection,
    organization: &OrganizationId,
    id: &str,
) -> Result<Option<CreditEntry>, BillingStoreError> {
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT amount_micro, payload FROM credit_entry
             WHERE organization_id = ?1 AND id = ?2",
            params![organization.as_str(), id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(backend)?;
    match row {
        Some((amount, payload)) => Ok(Some(decode_credit_row(id, amount, &payload)?)),
        None => Ok(None),
    }
}

/// The FIRST entry of `kind` naming `reference` (tenant-scoped). Used by the
/// append guards (already-settled, settle-after-refund), never to fold.
fn credit_entry_for_reference(
    conn: &Connection,
    organization: &OrganizationId,
    reference: &str,
    kind: CreditKind,
) -> Result<Option<CreditEntry>, BillingStoreError> {
    let row: Option<(String, i64, String)> = conn
        .query_row(
            "SELECT id, amount_micro, payload FROM credit_entry
             WHERE organization_id = ?1 AND reference = ?2 AND kind = ?3
             ORDER BY entry_seq LIMIT 1",
            params![organization.as_str(), reference, kind.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(backend)?;
    match row {
        Some((id, amount, payload)) => Ok(Some(decode_credit_row(&id, amount, &payload)?)),
        None => Ok(None),
    }
}

/// Every entry of `kind` naming `reference`, in durable order.
fn credit_entries_for_reference(
    conn: &Connection,
    organization: &OrganizationId,
    reference: &str,
    kind: CreditKind,
) -> Result<Vec<CreditEntry>, BillingStoreError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, amount_micro, payload FROM credit_entry
             WHERE organization_id = ?1 AND reference = ?2 AND kind = ?3
             ORDER BY entry_seq",
        )
        .map_err(backend)?;
    let rows = stmt
        .query_map(
            params![organization.as_str(), reference, kind.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(backend)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(backend)?;
    let mut out = Vec::with_capacity(rows.len());
    for (id, amount, payload) in rows {
        out.push(decode_credit_row(&id, amount, &payload)?);
    }
    Ok(out)
}

/// Decode one `credit_entry` row and verify its durable amount projection.
///
/// Invariant (P1 ledger-consistency): the stored `amount_micro` column is
/// the EXACT signed projection of the payload's `amount_micro`, whose domain
/// is bounded by [`crate::billing::MAX_CREDIT_AMOUNT_MICRO`] (`i64::MAX`), so
/// column and payload are bit-identical for every row the current writer
/// produces. A row written by the old clamping writer can still hold the
/// sentinel `i64::MAX` while the payload carries the true (out-of-domain)
/// amount. Such a row is REFUSED typed here, naming the row id and both
/// values — never silently reinterpreted as the clamped `i64::MAX`, and
/// never folded into a balance/audit/aggregation number.
///
/// No in-place migration rewrites these rows: the true amount is outside the
/// bounded domain, so there is no exact in-domain value to migrate to and
/// any rewrite would be a guess. The refusal names the row so an operator
/// can verify the ledger; this is the documented decision (the v6 task-id
/// precedent: refuse rather than guess).
fn decode_credit_row(
    id: &str,
    stored_amount: i64,
    payload: &str,
) -> Result<CreditEntry, BillingStoreError> {
    let entry: CreditEntry = parse(payload)?;
    if u64::try_from(stored_amount).ok() != Some(entry.amount_micro) {
        return Err(BillingStoreError::Malformed(format!(
            "credit_entry {id}: stored amount_micro {stored_amount} disagrees with the payload \
             amount_micro {} (legacy clamped row written before the i64::MAX domain bound); \
             refusing to reinterpret the ledger amount (operator verification required)",
            entry.amount_micro
        )));
    }
    Ok(entry)
}

// ------------------------------------------------- billing aggregate rollups
//
// The append-only ledgers stay the authority. These projections are updated
// in the SAME transaction as every append and are caught up lazily from the
// ledger watermark, so a read is O(1) (totals/balance) or O(tasks) (fold)
// instead of O(events/credits) per request.

/// One usage bucket key: the reversible task-id text plus the run id.
type UsageBucketKey = (String, String);

#[derive(Clone, Copy)]
enum RollupSign {
    Add,
    Subtract,
}

fn rollup_u64(field: &str, raw: &str) -> Result<u64, BillingStoreError> {
    raw.parse::<u64>().map_err(|_| {
        BillingStoreError::Malformed(format!(
            "billing rollup column {field} carries the non-u64 value {raw:?}"
        ))
    })
}

fn usage_totals_from_row(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> Result<UsageTotals, BillingStoreError> {
    let mut fields = Vec::with_capacity(10);
    for index in 0..10 {
        fields.push(row.get::<_, String>(offset + index).map_err(backend)?);
    }
    let totals = UsageTotals {
        input_tokens: rollup_u64("input_tokens", &fields[0])?,
        output_tokens: rollup_u64("output_tokens", &fields[1])?,
        cache_read_tokens: rollup_u64("cache_read_tokens", &fields[2])?,
        cache_write_tokens: rollup_u64("cache_write_tokens", &fields[3])?,
        reasoning_tokens: rollup_u64("reasoning_tokens", &fields[4])?,
        provider_cost_micro: rollup_u64("provider_cost_micro", &fields[5])?,
        managed_cost_micro: rollup_u64("managed_cost_micro", &fields[6])?,
        byok_cost_micro: rollup_u64("byok_cost_micro", &fields[7])?,
        events: rollup_u64("events", &fields[8])?,
        corrected_events: rollup_u64("corrected_events", &fields[9])?,
    };
    Ok(totals)
}

const USAGE_TOTAL_COLUMNS: &str = "input_tokens, output_tokens, cache_read_tokens, \
     cache_write_tokens, reasoning_tokens, provider_cost_micro, managed_cost_micro, \
     byok_cost_micro, events, corrected_events";

fn read_usage_projection(
    conn: &Connection,
    organization: &OrganizationId,
) -> Result<Option<(UsageTotals, i64)>, BillingStoreError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {USAGE_TOTAL_COLUMNS}, watermark FROM billing_usage_total
             WHERE organization_id = ?1"
        ))
        .map_err(backend)?;
    let mut rows = stmt
        .query(params![organization.as_str()])
        .map_err(backend)?;
    match rows.next().map_err(backend)? {
        Some(row) => {
            let totals = usage_totals_from_row(row, 0)?;
            let watermark: i64 = row.get(10).map_err(backend)?;
            Ok(Some((totals, watermark)))
        }
        None => Ok(None),
    }
}

fn read_usage_buckets(
    conn: &Connection,
    organization: &OrganizationId,
) -> Result<BTreeMap<UsageBucketKey, UsageTotals>, BillingStoreError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT task_id, run_id, {USAGE_TOTAL_COLUMNS} FROM billing_usage_task
             WHERE organization_id = ?1 ORDER BY task_id, run_id"
        ))
        .map_err(backend)?;
    let mut rows = stmt
        .query(params![organization.as_str()])
        .map_err(backend)?;
    let mut out = BTreeMap::new();
    while let Some(row) = rows.next().map_err(backend)? {
        let task_text: String = row.get(0).map_err(backend)?;
        let run_id: String = row.get(1).map_err(backend)?;
        let task_id = task_id_from_text(&task_text).ok_or_else(|| {
            BillingStoreError::Malformed(format!(
                "billing rollup task bucket carries the non-canonical task id {task_text:?}"
            ))
        })?;
        out.insert(
            (task_id_text(task_id), run_id),
            usage_totals_from_row(row, 2)?,
        );
    }
    Ok(out)
}

fn combine_totals(
    target: &mut UsageTotals,
    delta: &UsageTotals,
    sign: RollupSign,
    context: &str,
) -> Result<(), BillingStoreError> {
    fn combine(
        field: &'static str,
        left: &mut u64,
        right: u64,
        sign: RollupSign,
        context: &str,
    ) -> Result<(), BillingStoreError> {
        *left = match sign {
            RollupSign::Add => checked_ledger_sum(field, *left, right)?,
            RollupSign::Subtract => left.checked_sub(right).ok_or_else(|| {
                BillingStoreError::Ledger(CreditLedgerError::Corrupt {
                    detail: format!(
                        "billing usage rollup {field} underflow while reconciling {context}"
                    ),
                })
            })?,
        };
        Ok(())
    }
    for (field, left, right) in [
        ("input_tokens", &mut target.input_tokens, delta.input_tokens),
        (
            "output_tokens",
            &mut target.output_tokens,
            delta.output_tokens,
        ),
        (
            "cache_read_tokens",
            &mut target.cache_read_tokens,
            delta.cache_read_tokens,
        ),
        (
            "cache_write_tokens",
            &mut target.cache_write_tokens,
            delta.cache_write_tokens,
        ),
        (
            "reasoning_tokens",
            &mut target.reasoning_tokens,
            delta.reasoning_tokens,
        ),
        (
            "provider_cost_micro",
            &mut target.provider_cost_micro,
            delta.provider_cost_micro,
        ),
        (
            "managed_cost_micro",
            &mut target.managed_cost_micro,
            delta.managed_cost_micro,
        ),
        (
            "byok_cost_micro",
            &mut target.byok_cost_micro,
            delta.byok_cost_micro,
        ),
        ("events", &mut target.events, delta.events),
        (
            "corrected_events",
            &mut target.corrected_events,
            delta.corrected_events,
        ),
    ] {
        combine(field, left, right, sign, context)?;
    }
    Ok(())
}

fn event_values(event: &UsageEvent) -> UsageTotals {
    let mut values = UsageTotals {
        events: 1,
        ..UsageTotals::default()
    };
    match event.unit {
        UsageUnit::InputTokens => values.input_tokens = event.quantity,
        UsageUnit::OutputTokens => values.output_tokens = event.quantity,
        UsageUnit::CacheReadTokens => values.cache_read_tokens = event.quantity,
        UsageUnit::CacheWriteTokens => values.cache_write_tokens = event.quantity,
        UsageUnit::ReasoningTokens => values.reasoning_tokens = event.quantity,
        UsageUnit::ProviderCostMicro => {
            values.provider_cost_micro = event.provider_cost_micro;
            match event.category {
                SpendCategory::Managed => values.managed_cost_micro = event.provider_cost_micro,
                SpendCategory::Byok => values.byok_cost_micro = event.provider_cost_micro,
            }
        }
    }
    values
}

fn usage_event_by_id(
    conn: &Connection,
    organization: &OrganizationId,
    id: &str,
    period: Option<&str>,
) -> Result<Option<UsageEvent>, BillingStoreError> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM usage_event ue
             WHERE ue.organization_id = ?1 AND ue.id = ?2
               AND (?3 IS NULL OR ue.id IN (
                    SELECT event_id FROM usage_event_period
                    WHERE organization_id = ?1 AND period_id = ?3))",
            params![organization.as_str(), id, period],
            |row| row.get(0),
        )
        .optional()
        .map_err(backend)?;
    payload.map(|raw| parse(&raw)).transpose()
}

/// A correction event that names a target, with the identity fields the
/// fold orders by.
fn correction_winner(
    conn: &Connection,
    organization: &OrganizationId,
    base: &str,
    before_seq: i64,
    period: Option<&str>,
) -> Result<Option<UsageEvent>, BillingStoreError> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM usage_event ue INDEXED BY idx_usage_event_correction_winner
             WHERE ue.organization_id = ?1 AND ue.correction_of = ?2 AND ue.event_seq < ?3
               AND (?4 IS NULL OR ue.id IN (
                    SELECT event_id FROM usage_event_period
                    WHERE organization_id = ?1 AND period_id = ?4))
             ORDER BY ue.occurred_at_ms DESC, ue.id DESC LIMIT 1",
            params![organization.as_str(), base, before_seq, period],
            |row| row.get(0),
        )
        .optional()
        .map_err(backend)?;
    payload.map(|raw| parse(&raw)).transpose()
}

fn correction_targets(
    conn: &Connection,
    organization: &OrganizationId,
    id: &str,
    before_seq: i64,
    period: Option<&str>,
) -> Result<bool, BillingStoreError> {
    let present: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM usage_event ue INDEXED BY idx_usage_event_correction_winner
             WHERE ue.organization_id = ?1 AND ue.correction_of = ?2 AND ue.event_seq < ?3
               AND (?4 IS NULL OR ue.id IN (
                    SELECT event_id FROM usage_event_period
                    WHERE organization_id = ?1 AND period_id = ?4))
             LIMIT 1",
            params![organization.as_str(), id, before_seq, period],
            |row| row.get(0),
        )
        .optional()
        .map_err(backend)?;
    Ok(present.is_some())
}

/// Whether `node` currently contributes its values to the fold, simulating
/// the append order up to (but excluding) `before_seq`:
///
/// - an event targeted by ANY correction is superseded (the winning
///   correction owns the values);
/// - a correction additionally contributes only while it is the winning
///   correction of the event it names.
fn node_contributes(
    conn: &Connection,
    organization: &OrganizationId,
    node: &UsageEvent,
    before_seq: i64,
    period: Option<&str>,
) -> Result<bool, BillingStoreError> {
    if correction_targets(conn, organization, node.id.as_str(), before_seq, period)? {
        return Ok(false);
    }
    let Some(base) = &node.correction_of else {
        return Ok(true);
    };
    let winner = correction_winner(conn, organization, base.as_str(), before_seq, period)?;
    Ok(winner
        .as_ref()
        .map(|winner| winner.id == node.id)
        .unwrap_or(false))
}

fn bucket_for<'a>(
    buckets: &'a mut BTreeMap<UsageBucketKey, UsageTotals>,
    dirty: &mut std::collections::BTreeSet<UsageBucketKey>,
    key: UsageBucketKey,
) -> &'a mut UsageTotals {
    dirty.insert(key.clone());
    buckets.entry(key).or_default()
}

/// Apply one event to the in-memory projection exactly as the pure fold
/// would after this append. Corrections are reconciled against the events
/// already appended (`event_seq < seq`): the new correction either becomes
/// the winner (replacing the prior winner's or the target's contribution)
/// or is itself superseded.
#[allow(clippy::too_many_arguments)]
fn apply_usage_event(
    conn: &Connection,
    organization: &OrganizationId,
    event: &UsageEvent,
    seq: i64,
    period: Option<&str>,
    totals: &mut UsageTotals,
    buckets: &mut BTreeMap<UsageBucketKey, UsageTotals>,
    dirty: &mut std::collections::BTreeSet<UsageBucketKey>,
) -> Result<(), BillingStoreError> {
    let key = (task_id_text(event.task_id), event.run_id.clone());
    let Some(base_id) = &event.correction_of else {
        // A base event appended AFTER a correction that targets it is
        // already superseded in the pure fold (the fold has no append-order
        // dependency): it contributes only its corrected-event marker, never
        // its values. Without this check an out-of-order append would count
        // both the base and its correction.
        if correction_targets(conn, organization, event.id.as_str(), seq, period)? {
            let corrected = UsageTotals {
                corrected_events: 1,
                ..UsageTotals::default()
            };
            combine_totals(totals, &corrected, RollupSign::Add, "correction")?;
            combine_totals(
                bucket_for(buckets, dirty, key),
                &corrected,
                RollupSign::Add,
                "correction",
            )?;
            return Ok(());
        }
        let values = event_values(event);
        combine_totals(totals, &values, RollupSign::Add, "append")?;
        combine_totals(
            bucket_for(buckets, dirty, key),
            &values,
            RollupSign::Add,
            "append",
        )?;
        return Ok(());
    };
    // Every correction contributes one corrected event (whether it wins or
    // is superseded by a later correction).
    let corrected = UsageTotals {
        corrected_events: 1,
        ..UsageTotals::default()
    };
    combine_totals(totals, &corrected, RollupSign::Add, "correction")?;
    combine_totals(
        bucket_for(buckets, dirty, key.clone()),
        &corrected,
        RollupSign::Add,
        "correction",
    )?;

    let winner = correction_winner(conn, organization, base_id.as_str(), seq, period)?;
    let wins = winner
        .as_ref()
        .map(|winner| {
            (event.occurred_at_ms, event.id.as_str()) > (winner.occurred_at_ms, winner.id.as_str())
        })
        .unwrap_or(true);
    if !wins {
        return Ok(());
    }
    let values = event_values(event);
    combine_totals(totals, &values, RollupSign::Add, "correction")?;
    combine_totals(
        bucket_for(buckets, dirty, key),
        &values,
        RollupSign::Add,
        "correction",
    )?;
    match winner {
        Some(winner) => {
            if node_contributes(conn, organization, &winner, seq, period)? {
                let winner_key = (task_id_text(winner.task_id), winner.run_id.clone());
                let winner_values = event_values(&winner);
                combine_totals(totals, &winner_values, RollupSign::Subtract, "correction")?;
                combine_totals(
                    bucket_for(buckets, dirty, winner_key),
                    &winner_values,
                    RollupSign::Subtract,
                    "correction",
                )?;
            }
        }
        None => {
            let target = usage_event_by_id(conn, organization, base_id.as_str(), period)?;
            if let Some(target) = target {
                if node_contributes(conn, organization, &target, seq, period)? {
                    let target_key = (task_id_text(target.task_id), target.run_id.clone());
                    let target_values = event_values(&target);
                    combine_totals(totals, &target_values, RollupSign::Subtract, "correction")?;
                    combine_totals(
                        bucket_for(buckets, dirty, target_key),
                        &target_values,
                        RollupSign::Subtract,
                        "correction",
                    )?;
                    // The target moved from contributing to superseded: its
                    // corrected-event counter (and its bucket) increments —
                    // but a target that is ITSELF a correction already got
                    // its marker when it was appended. The pure fold counts
                    // such a superseded correction exactly once, so adding
                    // another marker here would over-count `corrected_events`.
                    if target.correction_of.is_none() {
                        let target_corrected = UsageTotals {
                            corrected_events: 1,
                            ..UsageTotals::default()
                        };
                        combine_totals(totals, &target_corrected, RollupSign::Add, "correction")?;
                        combine_totals(
                            bucket_for(
                                buckets,
                                dirty,
                                (task_id_text(target.task_id), target.run_id),
                            ),
                            &target_corrected,
                            RollupSign::Add,
                            "correction",
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn write_usage_total(
    conn: &Connection,
    organization: &OrganizationId,
    totals: &UsageTotals,
    watermark: i64,
) -> Result<(), BillingStoreError> {
    conn.execute(
        &format!(
            "INSERT INTO billing_usage_total
                (organization_id, {USAGE_TOTAL_COLUMNS}, watermark)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(organization_id) DO UPDATE SET
                input_tokens = excluded.input_tokens,
                output_tokens = excluded.output_tokens,
                cache_read_tokens = excluded.cache_read_tokens,
                cache_write_tokens = excluded.cache_write_tokens,
                reasoning_tokens = excluded.reasoning_tokens,
                provider_cost_micro = excluded.provider_cost_micro,
                managed_cost_micro = excluded.managed_cost_micro,
                byok_cost_micro = excluded.byok_cost_micro,
                events = excluded.events,
                corrected_events = excluded.corrected_events,
                watermark = excluded.watermark"
        ),
        params![
            organization.as_str(),
            totals.input_tokens.to_string(),
            totals.output_tokens.to_string(),
            totals.cache_read_tokens.to_string(),
            totals.cache_write_tokens.to_string(),
            totals.reasoning_tokens.to_string(),
            totals.provider_cost_micro.to_string(),
            totals.managed_cost_micro.to_string(),
            totals.byok_cost_micro.to_string(),
            totals.events.to_string(),
            totals.corrected_events.to_string(),
            watermark,
        ],
    )
    .map_err(backend)?;
    Ok(())
}

fn write_usage_bucket(
    conn: &Connection,
    organization: &OrganizationId,
    key: &UsageBucketKey,
    totals: &UsageTotals,
) -> Result<(), BillingStoreError> {
    conn.execute(
        &format!(
            "INSERT INTO billing_usage_task
                (organization_id, task_id, run_id, {USAGE_TOTAL_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(organization_id, task_id, run_id) DO UPDATE SET
                input_tokens = excluded.input_tokens,
                output_tokens = excluded.output_tokens,
                cache_read_tokens = excluded.cache_read_tokens,
                cache_write_tokens = excluded.cache_write_tokens,
                reasoning_tokens = excluded.reasoning_tokens,
                provider_cost_micro = excluded.provider_cost_micro,
                managed_cost_micro = excluded.managed_cost_micro,
                byok_cost_micro = excluded.byok_cost_micro,
                events = excluded.events,
                corrected_events = excluded.corrected_events"
        ),
        params![
            organization.as_str(),
            key.0,
            key.1,
            totals.input_tokens.to_string(),
            totals.output_tokens.to_string(),
            totals.cache_read_tokens.to_string(),
            totals.cache_write_tokens.to_string(),
            totals.reasoning_tokens.to_string(),
            totals.provider_cost_micro.to_string(),
            totals.managed_cost_micro.to_string(),
            totals.byok_cost_micro.to_string(),
            totals.events.to_string(),
            totals.corrected_events.to_string(),
        ],
    )
    .map_err(backend)?;
    Ok(())
}

fn write_usage_projection(
    conn: &Connection,
    organization: &OrganizationId,
    totals: &UsageTotals,
    watermark: i64,
    buckets: &BTreeMap<UsageBucketKey, UsageTotals>,
    dirty: &std::collections::BTreeSet<UsageBucketKey>,
) -> Result<(), BillingStoreError> {
    write_usage_total(conn, organization, totals, watermark)?;
    for key in dirty {
        let totals = buckets.get(key).cloned().unwrap_or_default();
        write_usage_bucket(conn, organization, key, &totals)?;
    }
    Ok(())
}

fn read_usage_bucket(
    conn: &Connection,
    organization: &OrganizationId,
    key: &UsageBucketKey,
) -> Result<Option<UsageTotals>, BillingStoreError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {USAGE_TOTAL_COLUMNS} FROM billing_usage_task
             WHERE organization_id = ?1 AND task_id = ?2 AND run_id = ?3"
        ))
        .map_err(backend)?;
    let mut rows = stmt
        .query(params![organization.as_str(), key.0, key.1])
        .map_err(backend)?;
    match rows.next().map_err(backend)? {
        Some(row) => Ok(Some(usage_totals_from_row(row, 0)?)),
        None => Ok(None),
    }
}

/// The live-append projection update: read only the totals row and the
/// buckets the event can touch (its own, plus a correction's prior winner and
/// target), so one metering append stays O(1) even at 10k task buckets.
fn apply_usage_event_pointwise(
    conn: &Connection,
    organization: &OrganizationId,
    event: &UsageEvent,
    seq: i64,
) -> Result<(), BillingStoreError> {
    let (mut totals, _) = read_usage_projection(conn, organization)?.unwrap_or_default();
    let mut buckets = BTreeMap::new();
    let mut dirty = std::collections::BTreeSet::new();
    let event_key = (task_id_text(event.task_id), event.run_id.clone());
    if let Some(values) = read_usage_bucket(conn, organization, &event_key)? {
        buckets.insert(event_key.clone(), values);
    }
    if let Some(base_id) = &event.correction_of {
        let winner = correction_winner(conn, organization, base_id.as_str(), seq, None)?;
        if let Some(winner) = &winner {
            let key = (task_id_text(winner.task_id), winner.run_id.clone());
            if let Some(values) = read_usage_bucket(conn, organization, &key)? {
                buckets.insert(key, values);
            }
        }
        if let Some(target) = usage_event_by_id(conn, organization, base_id.as_str(), None)? {
            let key = (task_id_text(target.task_id), target.run_id.clone());
            if let Some(values) = read_usage_bucket(conn, organization, &key)? {
                buckets.insert(key, values);
            }
        }
    }
    apply_usage_event(
        conn,
        organization,
        event,
        seq,
        None,
        &mut totals,
        &mut buckets,
        &mut dirty,
    )?;
    write_usage_total(conn, organization, &totals, seq)?;
    for (key, totals) in &buckets {
        write_usage_bucket(conn, organization, key, totals)?;
    }
    Ok(())
}

/// Catch the projection up to `usage_event_seq.next_seq`, then write only
/// the buckets whose values changed.
fn ensure_usage_rollup_tx(
    tx: &rusqlite::Transaction<'_>,
    organization: &OrganizationId,
) -> Result<(), BillingStoreError> {
    let (mut totals, watermark) = read_usage_projection(tx, organization)?.unwrap_or_default();
    let mut buckets = read_usage_buckets(tx, organization)?;
    let mut dirty = std::collections::BTreeSet::new();
    let mut cursor = watermark;
    loop {
        let page: Vec<(i64, String)> = {
            let mut stmt = tx
                .prepare(
                    "SELECT event_seq, payload FROM usage_event
                     WHERE organization_id = ?1 AND event_seq > ?2
                     ORDER BY event_seq LIMIT 512",
                )
                .map_err(backend)?;
            let mapped = stmt
                .query_map(params![organization.as_str(), cursor], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(backend)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(backend)?;
            mapped
        };
        if page.is_empty() {
            break;
        }
        for (seq, payload) in page {
            let event: UsageEvent = parse(&payload)?;
            // The SQL WHERE already scopes the row; a payload naming a
            // different organization is corrupt storage and must never be
            // folded into (or served as) this tenant's totals.
            if event.organization_id != *organization {
                return Err(BillingStoreError::Ledger(CreditLedgerError::Corrupt {
                    detail: format!(
                        "usage event {} in organization {} names organization {}",
                        event.id, organization, event.organization_id
                    ),
                }));
            }
            apply_usage_event(
                tx,
                organization,
                &event,
                seq,
                None,
                &mut totals,
                &mut buckets,
                &mut dirty,
            )?;
            cursor = seq;
        }
    }
    if cursor != watermark || dirty.is_empty() {
        write_usage_projection(tx, organization, &totals, cursor, &buckets, &dirty)?;
    }
    Ok(())
}

fn usage_next_seq(
    conn: &Connection,
    organization: &OrganizationId,
) -> Result<i64, BillingStoreError> {
    let seq: Option<i64> = conn
        .query_row(
            "SELECT next_seq FROM usage_event_seq WHERE organization_id = ?1",
            params![organization.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(backend)?;
    Ok(seq.unwrap_or(0))
}

/// The fast path: a projection whose watermark equals the ledger's next
/// sequence needs no transaction at all.
fn ensure_usage_rollup(
    conn: &mut Connection,
    organization: &OrganizationId,
) -> Result<(), BillingStoreError> {
    let target = usage_next_seq(conn, organization)?;
    if let Some((_, watermark)) = read_usage_projection(conn, organization)? {
        if watermark == target {
            return Ok(());
        }
    }
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(backend)?;
    ensure_usage_rollup_tx(&tx, organization)?;
    tx.commit().map_err(backend)?;
    Ok(())
}

fn read_usage_fold(
    conn: &Connection,
    organization: &OrganizationId,
) -> Result<UsageFold, BillingStoreError> {
    let (totals, _) = read_usage_projection(conn, organization)?.unwrap_or_default();
    let mut per_task = Vec::new();
    for ((task_text, run_id), totals) in read_usage_buckets(conn, organization)? {
        let task_id = task_id_from_text(&task_text).ok_or_else(|| {
            BillingStoreError::Malformed(format!(
                "billing rollup task bucket carries the non-canonical task id {task_text:?}"
            ))
        })?;
        per_task.push(TaskUsage {
            task_id,
            run_id,
            totals,
        });
    }
    Ok(UsageFold {
        organization_id: organization.clone(),
        totals,
        per_task,
        next_cursor: None,
    })
}

// --------------------------------------------------- credit balance rollup

fn credit_balance_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<(CreditBalance, i64), BillingStoreError> {
    let mut fields = Vec::with_capacity(5);
    for index in 0..5 {
        fields.push(row.get::<_, String>(index).map_err(backend)?);
    }
    let balance = CreditBalance {
        granted_micro: rollup_u64("granted_micro", &fields[0])?,
        consumed_micro: rollup_u64("consumed_micro", &fields[1])?,
        refunded_micro: rollup_u64("refunded_micro", &fields[2])?,
        held_micro: rollup_u64("held_micro", &fields[3])?,
        pending_consumes: rollup_u64("pending_consumes", &fields[4])?,
    };
    let watermark: i64 = row.get(5).map_err(backend)?;
    Ok((balance, watermark))
}

fn read_credit_projection(
    conn: &Connection,
    organization: &OrganizationId,
) -> Result<Option<(CreditBalance, i64)>, BillingStoreError> {
    let mut stmt = conn
        .prepare(
            "SELECT granted_micro, consumed_micro, refunded_micro, held_micro,
                    pending_consumes, watermark
             FROM billing_credit_rollup WHERE organization_id = ?1",
        )
        .map_err(backend)?;
    let mut rows = stmt
        .query(params![organization.as_str()])
        .map_err(backend)?;
    match rows.next().map_err(backend)? {
        Some(row) => Ok(Some(credit_balance_from_row(row)?)),
        None => Ok(None),
    }
}

fn write_credit_projection(
    conn: &Connection,
    organization: &OrganizationId,
    balance: &CreditBalance,
    watermark: i64,
) -> Result<(), BillingStoreError> {
    conn.execute(
        "INSERT INTO billing_credit_rollup
            (organization_id, granted_micro, consumed_micro, refunded_micro, held_micro,
             pending_consumes, watermark)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(organization_id) DO UPDATE SET
            granted_micro = excluded.granted_micro,
            consumed_micro = excluded.consumed_micro,
            refunded_micro = excluded.refunded_micro,
            held_micro = excluded.held_micro,
            pending_consumes = excluded.pending_consumes,
            watermark = excluded.watermark",
        params![
            organization.as_str(),
            balance.granted_micro.to_string(),
            balance.consumed_micro.to_string(),
            balance.refunded_micro.to_string(),
            balance.held_micro.to_string(),
            balance.pending_consumes.to_string(),
            watermark,
        ],
    )
    .map_err(backend)?;
    Ok(())
}

fn add_credit_delta(
    balance: &mut CreditBalance,
    field: &'static str,
    value: u64,
) -> Result<(), BillingStoreError> {
    match field {
        "granted_micro" => {
            balance.granted_micro = checked_ledger_sum(field, balance.granted_micro, value)?
        }
        "consumed_micro" => {
            balance.consumed_micro = checked_ledger_sum(field, balance.consumed_micro, value)?
        }
        "refunded_micro" => {
            balance.refunded_micro = checked_ledger_sum(field, balance.refunded_micro, value)?
        }
        "held_micro" => balance.held_micro = checked_ledger_sum(field, balance.held_micro, value)?,
        "pending_consumes" => {
            balance.pending_consumes = checked_ledger_sum(field, balance.pending_consumes, value)?
        }
        _ => unreachable!("credit rollup field"),
    }
    Ok(())
}

/// Apply one credit entry to the projection (the exact delta of
/// [`crate::billing::fold_credits`]): a grant funds, a consume debits and
/// holds, a settle replaces the hold with the actual (which may be lower),
/// and a refund funds.
fn apply_credit_entry(
    conn: &Connection,
    organization: &OrganizationId,
    balance: &mut CreditBalance,
    entry: &CreditEntry,
) -> Result<(), BillingStoreError> {
    if entry.organization != *organization {
        return Err(BillingStoreError::Ledger(CreditLedgerError::Corrupt {
            detail: format!(
                "credit entry {} in organization {} names organization {}",
                entry.id, organization, entry.organization
            ),
        }));
    }
    match entry.kind {
        CreditKind::Grant => add_credit_delta(balance, "granted_micro", entry.amount_micro),
        CreditKind::Consume => {
            add_credit_delta(balance, "consumed_micro", entry.amount_micro)?;
            add_credit_delta(balance, "held_micro", entry.amount_micro)?;
            add_credit_delta(balance, "pending_consumes", 1)
        }
        CreditKind::Settle => {
            let Some(reference) = &entry.reference else {
                return Ok(());
            };
            let target: Option<(String, i64)> = conn
                .query_row(
                    "SELECT kind, amount_micro FROM credit_entry
                     WHERE organization_id = ?1 AND id = ?2",
                    params![organization.as_str(), reference.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(backend)?;
            let Some((kind, held)) = target else {
                // The fold ignores a settle with no consume row; the append
                // path refuses one, so this is unreachable for written rows.
                return Ok(());
            };
            if kind != CreditKind::Consume.as_str() {
                return Ok(());
            }
            let held = u64::try_from(held).map_err(|_| {
                BillingStoreError::Malformed(format!(
                    "credit entry {} carries a negative amount_micro {held}",
                    reference.as_str()
                ))
            })?;
            balance.held_micro = balance.held_micro.checked_sub(held).ok_or_else(|| {
                BillingStoreError::Ledger(CreditLedgerError::Corrupt {
                    detail: format!(
                        "credit rollup held_micro is below the settle hold {held} of {}",
                        reference.as_str()
                    ),
                })
            })?;
            balance.pending_consumes =
                balance.pending_consumes.checked_sub(1).ok_or_else(|| {
                    BillingStoreError::Ledger(CreditLedgerError::Corrupt {
                        detail: format!(
                            "credit rollup pending_consumes is zero at the settle of {}",
                            reference.as_str()
                        ),
                    })
                })?;
            if entry.amount_micro >= held {
                add_credit_delta(
                    balance,
                    "consumed_micro",
                    entry.amount_micro.saturating_sub(held),
                )
            } else {
                let reduction = held - entry.amount_micro;
                balance.consumed_micro =
                    balance
                        .consumed_micro
                        .checked_sub(reduction)
                        .ok_or_else(|| {
                            BillingStoreError::Ledger(CreditLedgerError::Corrupt {
                                detail: format!(
                                    "credit rollup consumed_micro is below the settle reduction \
                                 {reduction} of {}",
                                    reference.as_str()
                                ),
                            })
                        })?;
                Ok(())
            }
        }
        CreditKind::Refund => {
            add_credit_delta(balance, "refunded_micro", entry.amount_micro)?;
            // A refund of a still-PENDING consume releases that much of its
            // hold (the pure `fold_credits` computes the same value from its
            // `released` map). A settled consume no longer holds anything:
            // its refund only funds the balance.
            let Some(reference) = &entry.reference else {
                return Ok(());
            };
            if credit_entry_for_reference(
                conn,
                organization,
                reference.as_str(),
                CreditKind::Settle,
            )?
            .is_some()
            {
                return Ok(());
            }
            let Some(target) = credit_entry_by_id(conn, organization, reference.as_str())? else {
                return Ok(());
            };
            if target.kind != CreditKind::Consume {
                return Ok(());
            }
            let mut released: u64 = 0;
            for row in credit_entries_for_reference(
                conn,
                organization,
                reference.as_str(),
                CreditKind::Refund,
            )? {
                released = checked_ledger_sum("refunded_micro", released, row.amount_micro)?;
            }
            let still_held = target.amount_micro.checked_sub(released).ok_or_else(|| {
                BillingStoreError::Ledger(CreditLedgerError::Corrupt {
                    detail: format!(
                        "pending consume {} has {released} microUSD released against its {} \
                         microUSD hold",
                        reference.as_str(),
                        target.amount_micro
                    ),
                })
            })?;
            balance.held_micro = balance
                .held_micro
                .checked_sub(entry.amount_micro)
                .ok_or_else(|| {
                    BillingStoreError::Ledger(CreditLedgerError::Corrupt {
                        detail: format!(
                            "credit rollup held_micro is below the released {} of consume {}",
                            entry.amount_micro,
                            reference.as_str()
                        ),
                    })
                })?;
            if still_held == 0 {
                balance.pending_consumes =
                    balance.pending_consumes.checked_sub(1).ok_or_else(|| {
                        BillingStoreError::Ledger(CreditLedgerError::Corrupt {
                            detail: format!(
                                "credit rollup pending_consumes is zero at the release of {}",
                                reference.as_str()
                            ),
                        })
                    })?;
            }
            Ok(())
        }
    }
}

fn credit_next_seq(
    conn: &Connection,
    organization: &OrganizationId,
) -> Result<i64, BillingStoreError> {
    let seq: Option<i64> = conn
        .query_row(
            "SELECT next_seq FROM billing_credit_seq WHERE organization_id = ?1",
            params![organization.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(backend)?;
    Ok(seq.unwrap_or(0))
}

fn ensure_credit_rollup_tx(
    tx: &rusqlite::Transaction<'_>,
    organization: &OrganizationId,
) -> Result<CreditBalance, BillingStoreError> {
    let (mut balance, watermark) = read_credit_projection(tx, organization)?.unwrap_or_default();
    let mut cursor = watermark;
    loop {
        let page: Vec<(i64, String, i64, String)> = {
            let mut stmt = tx
                .prepare(
                    "SELECT entry_seq, id, amount_micro, payload FROM credit_entry
                     WHERE organization_id = ?1 AND entry_seq > ?2
                     ORDER BY entry_seq LIMIT 512",
                )
                .map_err(backend)?;
            let mapped = stmt
                .query_map(params![organization.as_str(), cursor], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(backend)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(backend)?;
            mapped
        };
        if page.is_empty() {
            break;
        }
        for (seq, id, stored_amount, payload) in page {
            let entry = decode_credit_row(&id, stored_amount, &payload)?;
            apply_credit_entry(tx, organization, &mut balance, &entry)?;
            cursor = seq;
        }
    }
    if cursor != watermark {
        write_credit_projection(tx, organization, &balance, cursor)?;
    }
    Ok(balance)
}

impl BillingStore for SqliteControlPlaneStore {
    fn put_billing_account(&self, account: &BillingAccount) -> Result<(), BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        // An account id is globally unique. The guarded upsert updates only
        // when the existing row belongs to the SAME organization; a foreign
        // owner leaves zero rows changed and is a typed refusal (never a
        // silent reassignment of another tenant's ledger identity).
        let changed = conn
            .execute(
                "INSERT INTO billing_account (id, organization_id, payload) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET payload = excluded.payload
                 WHERE billing_account.organization_id = excluded.organization_id",
                params![
                    account.id.as_str(),
                    account.organization.as_str(),
                    encode(account)?
                ],
            )
            .map_err(backend)?;
        if changed == 0 {
            return Err(BillingStoreError::Malformed(format!(
                "billing account {} is already owned by another organization; refusing to \
                 reassign it",
                account.id
            )));
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
        let limit = checked_page_limit(limit)?;
        let conn = self.lock_billing_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT payload FROM billing_account
                 WHERE organization_id = ?1 AND (?2 IS NULL OR id > ?2)
                 ORDER BY id LIMIT ?3",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![organization.as_str(), after, limit], |r| {
                r.get::<_, String>(0)
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        rows.iter().map(|p| parse(p)).collect()
    }

    fn put_subscription(&self, subscription: &Subscription) -> Result<(), BillingStoreError> {
        subscription
            .validate()
            .map_err(|error| BillingStoreError::Malformed(error.to_string()))?;
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
        payload
            .map(|p| {
                let subscription: Subscription = parse(&p)?;
                if subscription.organization != *organization {
                    return Err(BillingStoreError::Malformed(format!(
                        "subscription {} in organization {} names organization {}",
                        subscription.id, organization, subscription.organization
                    )));
                }
                subscription
                    .validate()
                    .map_err(|error| BillingStoreError::Malformed(error.to_string()))?;
                Ok(subscription)
            })
            .transpose()
    }

    fn append_usage_event(&self, event: &UsageEvent) -> Result<UsageAppend, BillingStoreError> {
        event
            .validate()
            .map_err(|error| BillingStoreError::Malformed(error.to_string()))?;
        let mut conn = self.lock_billing_conn()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        let duplicate: Option<String> = tx
            .query_row(
                "SELECT payload FROM usage_event WHERE organization_id = ?1 AND source_key = ?2",
                params![event.organization_id.as_str(), event.source_key],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        if let Some(payload) = duplicate {
            let existing: UsageEvent = parse(&payload)?;
            tx.commit().map_err(backend)?;
            if existing.same_fact(event) {
                return Ok(UsageAppend::Duplicate);
            }
            return Err(BillingStoreError::Malformed(format!(
                "usage source key {:?} already records a different fact; refusing a silent \
                 duplicate",
                event.source_key
            )));
        }
        // The rollup reflects every prior event before the new row is
        // applied (a v7/restored projection is caught up once, here).
        let target = usage_next_seq(&tx, &event.organization_id)?;
        let current = read_usage_projection(&tx, &event.organization_id)?;
        if current.as_ref().map(|(_, watermark)| *watermark) != Some(target) {
            ensure_usage_rollup_tx(&tx, &event.organization_id)?;
        }
        let period = ensure_event_period_tx(&tx, &event.organization_id, event)?;
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
                crate::billing::task_id_text(event.task_id),
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
        apply_usage_event_pointwise(&tx, &event.organization_id, event, seq)?;
        // The per-period projection: steady state applies the new event
        // pointwise; a projection that is behind (migration/restore) is
        // caught up in the same transaction, never read through a prefix.
        if period_rollup_watermark(&tx, &event.organization_id)? == seq - 1 {
            apply_period_event_pointwise(&tx, &event.organization_id, event, seq, &period)?;
            tx.execute(
                "INSERT INTO billing_period_rollup_state (organization_id, watermark)
                 VALUES (?1, ?2)
                 ON CONFLICT(organization_id) DO UPDATE SET watermark = excluded.watermark",
                params![event.organization_id.as_str(), seq],
            )
            .map_err(backend)?;
        } else {
            ensure_period_rollup_tx(&tx, &event.organization_id)?;
        }
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
                "SELECT ue.event_seq, ue.payload FROM usage_event ue
                 WHERE ue.organization_id = ?1 AND ue.id = ?2",
                params![organization.as_str(), id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(backend)?;
        match row {
            Some((event_seq, payload)) => {
                let event: UsageEvent = parse(&payload)?;
                if event.organization_id != *organization {
                    return Err(BillingStoreError::Malformed(format!(
                        "usage event {} in organization {} names organization {}",
                        event.id, organization, event.organization_id
                    )));
                }
                let period_id = read_event_period(&conn, organization, event.id.as_str())?
                    .map(|p| p.id)
                    .unwrap_or_else(|| BillingPeriod::unconfigured().id);
                Ok(Some(StoredUsageEvent {
                    event_seq,
                    event,
                    period_id,
                }))
            }
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

    fn usage_fold(&self, organization: &OrganizationId) -> Result<UsageFold, BillingStoreError> {
        let mut conn = self.lock_billing_conn()?;
        ensure_usage_rollup(&mut conn, organization)?;
        read_usage_fold(&conn, organization)
    }

    fn usage_totals(
        &self,
        organization: &OrganizationId,
    ) -> Result<UsageTotals, BillingStoreError> {
        let mut conn = self.lock_billing_conn()?;
        ensure_usage_rollup(&mut conn, organization)?;
        Ok(read_usage_projection(&conn, organization)?
            .map(|(totals, _)| totals)
            .unwrap_or_default())
    }

    fn usage_fold_in_period(
        &self,
        organization: &OrganizationId,
        period_id: &str,
    ) -> Result<UsageFold, BillingStoreError> {
        let mut conn = self.lock_billing_conn()?;
        ensure_period_rollup(&mut conn, organization)?;
        read_period_fold(&conn, organization, period_id)
    }

    fn usage_totals_in_period(
        &self,
        organization: &OrganizationId,
        period_id: &str,
    ) -> Result<UsageTotals, BillingStoreError> {
        let mut conn = self.lock_billing_conn()?;
        ensure_period_rollup(&mut conn, organization)?;
        Ok(read_period_projection(&conn, organization, period_id)?.unwrap_or_default())
    }

    fn put_period_config(
        &self,
        organization: &OrganizationId,
        anchor_ms: i64,
        period_ms: i64,
    ) -> Result<(), BillingStoreError> {
        if anchor_ms < 0 || period_ms <= 0 || period_ms > faktor_core::MAX_BILLING_PERIOD_MS {
            return Err(BillingStoreError::Malformed(
                "period configuration is outside the legal domain".into(),
            ));
        }
        let conn = self.lock_billing_conn()?;
        conn.execute(
            "INSERT INTO billing_period_config
                (organization_id, anchor_ms, period_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(organization_id) DO UPDATE SET
                anchor_ms = excluded.anchor_ms,
                period_ms = excluded.period_ms",
            params![organization.as_str(), anchor_ms, period_ms],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn period_config(
        &self,
        organization: &OrganizationId,
    ) -> Result<Option<(i64, i64)>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        read_period_config(&conn, organization)
    }

    fn outbox_watermark(&self, organization: &OrganizationId) -> Result<i64, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        Ok(conn
            .query_row(
                "SELECT last_outbox_id FROM billing_outbox_watermark
                 WHERE organization_id = ?1",
                params![organization.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?
            .unwrap_or(0))
    }

    fn advance_outbox_watermark(
        &self,
        organization: &OrganizationId,
        last_outbox_id: i64,
    ) -> Result<(), BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        conn.execute(
            "INSERT INTO billing_outbox_watermark (organization_id, last_outbox_id)
             VALUES (?1, ?2)
             ON CONFLICT(organization_id) DO UPDATE SET
                last_outbox_id = MAX(billing_outbox_watermark.last_outbox_id, excluded.last_outbox_id)",
            params![organization.as_str(), last_outbox_id],
        )
        .map_err(backend)?;
        Ok(())
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
        let limit = checked_page_limit(limit)?;
        let conn = self.lock_billing_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT entry_seq, id, amount_micro, payload FROM credit_entry
                 WHERE organization_id = ?1 AND entry_seq > ?2
                 ORDER BY entry_seq LIMIT ?3",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![organization.as_str(), after_seq, limit], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        let mut out = Vec::with_capacity(rows.len());
        for (entry_seq, id, amount, payload) in rows {
            let entry = decode_credit_row(&id, amount, &payload)?;
            if entry.organization != *organization {
                return Err(BillingStoreError::Malformed(format!(
                    "credit entry {} in organization {} names organization {}",
                    entry.id, organization, entry.organization
                )));
            }
            out.push(StoredCreditEntry { entry_seq, entry });
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
                "SELECT entry_seq, id, amount_micro, payload FROM credit_entry
                 WHERE organization_id = ?1 AND idempotency_key = ?2",
                params![organization.as_str(), key],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(backend)?;
        match row {
            Some((entry_seq, id, amount, payload)) => {
                let entry = decode_credit_row(&id, amount, &payload)?;
                if entry.organization != *organization {
                    return Err(BillingStoreError::Malformed(format!(
                        "credit entry {} in organization {} names organization {}",
                        entry.id, organization, entry.organization
                    )));
                }
                Ok(Some(StoredCreditEntry { entry_seq, entry }))
            }
            None => Ok(None),
        }
    }

    fn credit_balance(
        &self,
        organization: &OrganizationId,
    ) -> Result<CreditBalance, BillingStoreError> {
        let mut conn = self.lock_billing_conn()?;
        let target = credit_next_seq(&conn, organization)?;
        if let Some((balance, watermark)) = read_credit_projection(&conn, organization)? {
            if watermark == target {
                return Ok(balance);
            }
        }
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        let balance = ensure_credit_rollup_tx(&tx, organization)?;
        tx.commit().map_err(backend)?;
        Ok(balance)
    }

    fn begin_in_flight(&self, txn: &InFlightTxn) -> Result<(), BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        // An in-flight id is tenant-scoped: a conflicting id owned by another
        // organization is NEVER overwritten (the guarded update touches zero
        // rows and refuses typed). The payload is the durable read shape, so
        // a cross-tenant overwrite would also inject a foreign organization
        // into the owner's in-flight view.
        let changed = conn
            .execute(
                "INSERT INTO billing_in_flight (id, organization_id, kind, reference, started_ms, ended_ms, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET payload = excluded.payload
                 WHERE billing_in_flight.organization_id = excluded.organization_id",
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
        if changed == 0 {
            return Err(BillingStoreError::Malformed(format!(
                "in-flight transaction {} is already owned by another organization; refusing a \
                 cross-tenant overwrite",
                txn.id
            )));
        }
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
        let mut out = Vec::with_capacity(rows.len());
        for payload in rows {
            let txn: InFlightTxn = parse(&payload)?;
            if txn.organization != *organization {
                return Err(BillingStoreError::Malformed(format!(
                    "in-flight transaction {} in organization {} names organization {}",
                    txn.id, organization, txn.organization
                )));
            }
            out.push(txn);
        }
        Ok(out)
    }

    fn claim_billing_mutation(
        &self,
        organization: &OrganizationId,
        key: &str,
        request: &str,
        at_ms: i64,
    ) -> Result<BillingMutationClaim, BillingStoreError> {
        if request.len() > MAX_BILLING_MUTATION_BODY {
            return Err(BillingStoreError::Malformed(
                "commercial mutation request exceeds the idempotency body bound".into(),
            ));
        }
        let mut conn = self.lock_billing_conn()?;
        conn.execute_batch(BILLING_MUTATION_SCHEMA)
            .map_err(backend)?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        let existing: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT request, response FROM billing_mutation
                 WHERE organization_id = ?1 AND idempotency_key = ?2",
                params![organization.as_str(), key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(backend)?;
        if let Some((existing_request, response)) = existing {
            tx.commit().map_err(backend)?;
            if existing_request == request {
                return Ok(BillingMutationClaim::Replay { response });
            }
            return Ok(BillingMutationClaim::Conflict {
                request: existing_request,
            });
        }
        tx.execute(
            "INSERT INTO billing_mutation
                (organization_id, idempotency_key, request, response, created_ms)
             VALUES (?1, ?2, ?3, NULL, ?4)",
            params![organization.as_str(), key, request, at_ms],
        )
        .map_err(backend)?;
        // Bounded schedule: keep only the newest keys for this organization
        // (caller-chosen keys must never grow the durable table without
        // bound). Older keys lose their replay record, never correctness.
        tx.execute(
            "DELETE FROM billing_mutation WHERE organization_id = ?1 AND idempotency_key NOT IN (
                 SELECT idempotency_key FROM billing_mutation
                 WHERE organization_id = ?1
                 ORDER BY rowid DESC
                 LIMIT ?2)",
            params![organization.as_str(), MAX_BILLING_MUTATIONS_PER_ORG as i64],
        )
        .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(BillingMutationClaim::Fresh)
    }

    fn billing_mutation_response(
        &self,
        organization: &OrganizationId,
        key: &str,
    ) -> Result<Option<String>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        conn.execute_batch(BILLING_MUTATION_SCHEMA)
            .map_err(backend)?;
        conn.query_row(
            "SELECT response FROM billing_mutation
             WHERE organization_id = ?1 AND idempotency_key = ?2",
            params![organization.as_str(), key],
            |r| r.get(0),
        )
        .optional()
        .map_err(backend)
    }

    fn finish_billing_mutation(
        &self,
        organization: &OrganizationId,
        key: &str,
        response: &str,
    ) -> Result<(), BillingStoreError> {
        if response.len() > MAX_BILLING_MUTATION_BODY {
            return Err(BillingStoreError::Malformed(
                "commercial mutation response exceeds the idempotency body bound".into(),
            ));
        }
        let conn = self.lock_billing_conn()?;
        conn.execute_batch(BILLING_MUTATION_SCHEMA)
            .map_err(backend)?;
        let updated = conn
            .execute(
                "UPDATE billing_mutation SET response = ?1
                 WHERE organization_id = ?2 AND idempotency_key = ?3",
                params![response, organization.as_str(), key],
            )
            .map_err(backend)?;
        if updated != 1 {
            return Err(BillingStoreError::Malformed(
                "an idempotency key was finished before it was claimed".into(),
            ));
        }
        Ok(())
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
        // bound (reported/failed rows first at the same age; open periods
        // and skipped audit rows are retained preferentially).
        tx.execute(
            "DELETE FROM billing_report_period WHERE organization_id = ?1 AND period NOT IN (
                 SELECT period FROM billing_report_period WHERE organization_id = ?1
                 ORDER BY (status IN ('open', 'skipped')) DESC, first_seen_ms DESC, period DESC
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
        let limit = checked_page_limit(limit)?;
        let conn = self.lock_billing_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT payload FROM billing_report_period
                 WHERE organization_id = ?1
                 ORDER BY first_seen_ms DESC, period DESC LIMIT ?2",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![organization.as_str(), limit], |r| {
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
        let limit = checked_page_limit(limit)?;
        let conn = self.lock_billing_conn()?;
        // The task filter compares the reversible text encoding
        // (`crate::billing::task_id_text`), NEVER a numeric projection: the
        // old `task_id.min(i64::MAX as u64) as i64` comparison aliased every
        // id >= i64::MAX onto one value, so a filtered page could return
        // another task's rows. Two statements (rather than an
        // `? IS NULL OR task_id = ?` predicate) keep the
        // `(organization_id, task_id, event_seq)` index applicable to the
        // filtered page.
        let rows: Vec<(i64, String, Option<String>)> = match task_id {
            Some(task_id) => {
                let mut stmt = conn
                    .prepare(
                        "SELECT ue.event_seq, ue.payload, ep.period_id
                         FROM usage_event ue
                         LEFT JOIN usage_event_period ep
                           ON ep.event_id = ue.id AND ep.organization_id = ue.organization_id
                         WHERE ue.organization_id = ?1 AND ue.task_id = ?2 AND ue.event_seq > ?3
                         ORDER BY ue.event_seq LIMIT ?4",
                    )
                    .map_err(backend)?;
                let mapped = stmt
                    .query_map(
                        params![
                            organization.as_str(),
                            crate::billing::task_id_text(task_id),
                            after_seq,
                            limit
                        ],
                        usage_row,
                    )
                    .map_err(backend)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(backend)?;
                mapped
            }
            None => {
                let mut stmt = conn
                    .prepare(
                        "SELECT ue.event_seq, ue.payload, ep.period_id
                         FROM usage_event ue
                         LEFT JOIN usage_event_period ep
                           ON ep.event_id = ue.id AND ep.organization_id = ue.organization_id
                         WHERE ue.organization_id = ?1 AND ue.event_seq > ?2
                         ORDER BY ue.event_seq LIMIT ?3",
                    )
                    .map_err(backend)?;
                let mapped = stmt
                    .query_map(params![organization.as_str(), after_seq, limit], usage_row)
                    .map_err(backend)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(backend)?;
                mapped
            }
        };
        let mut out = Vec::with_capacity(rows.len());
        for (event_seq, payload, period_id) in rows {
            let event: UsageEvent = parse(&payload)?;
            if event.organization_id != *organization {
                return Err(BillingStoreError::Malformed(format!(
                    "usage event {} in organization {} names organization {}",
                    event.id, organization, event.organization_id
                )));
            }
            out.push(StoredUsageEvent {
                event_seq,
                event,
                period_id: period_id.unwrap_or_else(|| BillingPeriod::unconfigured().id),
            });
        }
        Ok(out)
    }
}

/// Decode one `(event_seq, payload)` usage row.
fn usage_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, String, Option<String>)> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
}
